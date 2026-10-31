//! jarvis façade — thin delegates to services.

use super::ChatterClient;
use crate::domain::channel::{Channel, ChannelType};

/// Display name of the virtual assistant channel.
pub const JARVIS_CHANNEL_NAME: &str = "Jarvis (AI)";

/// The virtual assistant channel's fixed id: the nil UUID. Matches the
/// `JarvisContext::general()` context key, so channel history and Jarvis
/// memory share one identity. Never collides with server channels (v4 UUIDs).
pub fn jarvis_channel_id() -> crate::domain::ids::ChannelId {
    crate::domain::ids::ChannelId::from_uuid(uuid::Uuid::nil())
}

/// Whether an id refers to the virtual assistant channel.
pub fn is_jarvis_channel(id: &crate::domain::ids::ChannelId) -> bool {
    id.0.is_nil()
}

/// Resolve the typed conversation once for every inline Jarvis entry point.
pub fn jarvis_context_for(
    active: &crate::state::ActiveConversation,
) -> Option<crate::domain::jarvis::JarvisContext> {
    use crate::domain::jarvis::JarvisContext;
    use crate::state::ActiveConversation as AC;
    match active {
        AC::Channel(id) => Some(JarvisContext::channel(*id)),
        AC::ServerDm { .. } => Some(JarvisContext::dm(active.channel_id()?)),
        AC::AnonDm(id) => Some(JarvisContext::anon_dm(*id)),
        AC::AnonGroup(id) => Some(JarvisContext::anon_group(*id)),
        AC::None => None,
    }
}

/// Account identity captured before an asynchronous Jarvis command starts.
#[derive(Clone, Copy)]
pub struct JarvisSessionOwner {
    pub user_id: crate::domain::ids::UserId,
    pub revision: u64,
}

/// The query of an `@jarvis` mention, if `text` has one. The mention must be a
/// whole word (start of text or after whitespace; end of text, whitespace or
/// `,:;!?` after it) — `me@jarvis.io` is an address, not a mention. Leading
/// punctuation and whitespace are trimmed off the query.
pub fn parse_jarvis_mention(text: &str) -> Option<&str> {
    const TAG: &str = "@jarvis";
    for (at, _) in text.match_indices('@') {
        let starts_word = text[..at]
            .chars()
            .next_back()
            .map_or(true, char::is_whitespace);
        let rest = &text[at..];
        if !starts_word
            || !rest
                .get(..TAG.len())
                .is_some_and(|t| t.eq_ignore_ascii_case(TAG))
        {
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
        let query = after
            .trim_start_matches(|c: char| c.is_whitespace() || ",:;!?".contains(c))
            .trim_end();
        return (!query.is_empty()).then_some(query);
    }
    None
}

/// A Jarvis message row as core shapes it: who, what, when. Jarvis turns
/// are stored in the device assistant history rather than the message store; the UI only
/// formats the time and builds its view type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JarvisRow {
    pub id: String,
    /// The user's own turn (else Jarvis speaking).
    pub is_own: bool,
    pub sender_name: String,
    pub content: String,
    pub timestamp_ns: u64,
    /// The "thinking" placeholder shown while a reply is generated.
    pub thinking: bool,
}

const JARVIS_NAME: &str = "Jarvis";

fn now_ns() -> u64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64
}

impl JarvisRow {
    fn jarvis(id: String, content: String, timestamp_ns: u64) -> Self {
        Self {
            id,
            is_own: false,
            sender_name: JARVIS_NAME.into(),
            content,
            timestamp_ns,
            thinking: false,
        }
    }

    /// Placeholder while Jarvis generates a reply; replaced by its id.
    pub fn thinking() -> Self {
        Self {
            thinking: true,
            ..Self::jarvis(
                format!("jarvis-thinking-{}", uuid::Uuid::new_v4()),
                "_Thinking..._".into(),
                now_ns(),
            )
        }
    }

    /// A Jarvis answer shown inline in a normal conversation (mention or
    /// slash command): the question is quoted above the answer.
    pub fn inline_reply(query: &str, response: &str) -> Self {
        Self::jarvis(
            format!("jarvis-{}", uuid::Uuid::new_v4()),
            format!("**Q:** {}\n\n{}", query, response),
            now_ns(),
        )
    }

    /// A Jarvis turn in the AI channel.
    pub fn reply(response: &str) -> Self {
        Self::jarvis(
            format!("jarvis-{}", uuid::Uuid::new_v4()),
            response.to_string(),
            now_ns(),
        )
    }

    /// The user's own turn in the AI channel.
    pub fn own(own_name: &str, text: &str) -> Self {
        Self {
            id: format!("jarvis-user-{}", uuid::Uuid::new_v4()),
            is_own: true,
            sender_name: own_name.to_string(),
            content: text.to_string(),
            timestamp_ns: now_ns(),
            thinking: false,
        }
    }

    fn from_history(c: &crate::domain::jarvis::JarvisConversation, own_name: &str) -> Self {
        let is_own = matches!(c.role, crate::domain::jarvis::Role::User);
        Self {
            id: c.id.to_string(),
            is_own,
            sender_name: if is_own { own_name } else { JARVIS_NAME }.to_string(),
            content: c.content.clone(),
            timestamp_ns: c.timestamp,
            thinking: false,
        }
    }
}

/// What the AI channel shows for a finished turn: the reply, or the error
/// with a pointer to where the provider is configured.
pub fn jarvis_channel_reply_text(
    result: &Result<String, crate::services::jarvis::JarvisError>,
) -> String {
    match result {
        Ok(r) => r.clone(),
        Err(e) => format!(
            "Error: {}\n\nCheck Settings → Remote AI and configure a provider.",
            e
        ),
    }
}

impl ChatterClient {
    // ── Jarvis façade ────────────────────────────────────────────────────

    /// The current user's name on their own AI-channel turns.
    pub fn jarvis_own_name(&self) -> String {
        self.auth_state()
            .get()
            .display_name
            .unwrap_or_else(|| "Me".to_string())
    }

    /// The AI channel's persisted history as rows (chronological).
    pub fn jarvis_channel_rows(&self, limit: u32) -> Vec<JarvisRow> {
        let own_name = self.jarvis_own_name();
        match self.jarvis_channel_history(limit) {
            Ok(history) => history
                .iter()
                .map(|c| JarvisRow::from_history(c, &own_name))
                .collect(),
            Err(e) => {
                tracing::error!("failed to load AI channel history: {e}");
                Vec::new()
            }
        }
    }

    /// Whether the virtual assistant channel should appear in the sidebar.
    ///
    /// Follows explicit remote-AI consent; a missing provider surfaces as
    /// a helpful error inside the channel rather than hiding it, so users
    /// can discover the feature and be pointed at settings.
    pub fn jarvis_channel_enabled(&self) -> bool {
        self.remote_ai().is_enabled()
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
    /// go. Unconfigured or disabled remote AI never claims a local model exists.
    pub fn jarvis_channel_topic(&self) -> String {
        self.jarvis_privacy_line().0
    }

    /// The AI channel's privacy line: the same statement as its topic, and
    /// whether its messages leave this device (to a cloud provider, not end-
    /// to-end encrypted). Its kind — a private channel — would otherwise
    /// claim "Messages are end-to-end encrypted", which is false either way.
    pub fn jarvis_privacy_line(&self) -> (String, bool) {
        jarvis_privacy_for(
            self.jarvis_channel_silo(),
            &self.jarvis_engine_description(),
        )
    }

    /// Silo for the AI channel and the context-free global prompt: the
    /// user's preference, cloud only when it is also configured.
    fn jarvis_channel_silo(&self) -> crate::ai_policy::policy::Silo {
        use crate::ai_policy::channel_ai::SiloPreference;
        use crate::ai_policy::policy::Silo;
        let prefers_cloud = self.ai_silo_preference() == SiloPreference::PreferCloud;
        let available = self.remote_ai().cloud_settings().is_available();
        if prefers_cloud && available {
            Silo::Cloud
        } else {
            Silo::Local
        }
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
        let query = parse_jarvis_mention(text)?;
        let context = jarvis_context_for(active)?;
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
        let Some(owner) = self.auth_state().get().current_user_id else {
            return Ok(Vec::new());
        };
        let context = crate::domain::jarvis::JarvisContext::general().for_remote(&owner);
        self.jarvis().get_conversation_history(&context, limit)
    }

    /// Install the cloud engine for the configured mode, or clear it.
    ///
    /// Missing configuration clears the remote engine; private conversations
    /// remain unavailable and never fall back to local inference.
    pub fn rebuild_cloud_engine(&self) {
        use crate::cloud_ai::{AnyCloudBackend, CloudMode};
        use crate::services::jarvis::JarvisEngine;

        let cloud = self.remote_ai().cloud_settings();
        let engine = match cloud.mode {
            // Managed tokens: the server holds the key, so this needs a live
            // authenticated client rather than settings.
            CloudMode::ServerBroker => Some(JarvisEngine::cloud(
                AnyCloudBackend::broker(self.services.grpc_pool.ai_client()),
                cloud.model.clone(),
            )),
            // Bring-your-own-key: absent or blank key means no cloud engine.
            CloudMode::DirectAnthropic => JarvisEngine::from_cloud_settings(&cloud),
        };

        let configured = engine.is_some();
        self.services.jarvis_engine.set_cloud(engine);
        tracing::info!(mode = %cloud.mode.as_str(), configured, "cloud jarvis engine rebuilt");
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

    /// Header line for the memory browser: the AI channel's stored turns and
    /// summaries plus the real fact and entity totals.
    pub fn jarvis_memory_stats_label(&self) -> String {
        let facts = self.jarvis_memory_facts();
        let current = facts.iter().filter(|f| f.valid_until.is_none()).count();
        match self
            .jarvis()
            .get_memory_stats(&crate::domain::jarvis::JarvisContext::general())
        {
            Ok(stats) => memory_stats_label(&stats, facts.len(), current),
            Err(e) => {
                tracing::warn!("Jarvis memory stats unavailable: {e}");
                String::new()
            }
        }
    }

    /// Delete the AI channel's conversation history and its summaries.
    /// Facts and the knowledge graph stay. Returns the turns deleted.
    pub fn jarvis_clear_history(&self) -> Result<u32, crate::services::jarvis::JarvisError> {
        let Some(owner) = self.auth_state().get().current_user_id else {
            return Ok(0);
        };
        let context = crate::domain::jarvis::JarvisContext::general().for_remote(&owner);
        self.jarvis().clear_memory(&context)
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
        if silo == crate::ai_policy::policy::Silo::Cloud && !self.services.jarvis_engine.has_cloud()
        {
            self.rebuild_cloud_engine();
        }
        let reply = self
            .jarvis()
            .ask_in(context, query, author_id, silo)
            .await?;
        // No automatic local extraction, indexing or extra summary requests.
        Ok(reply)
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
        if context.context_id.is_empty() || context.context_id == jarvis_channel_id().to_string() {
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
        self.jarvis_execute_in_context(cmd, context, author_id, None)
            .await
    }

    /// Execute an inline slash command for the captured account and conversation.
    pub async fn jarvis_execute_for_conversation(
        &self,
        cmd: crate::services::jarvis::JarvisCommand,
        conversation: crate::state::ActiveConversation,
        owner: JarvisSessionOwner,
    ) -> (String, Result<String, crate::services::jarvis::JarvisError>) {
        use crate::services::jarvis::JarvisError;
        let Some(context) = jarvis_context_for(&conversation) else {
            return (
                String::new(),
                Err(JarvisError::ContextBuilding(
                    "No conversation selected".into(),
                )),
            );
        };
        if !self.jarvis_session_is_current(owner) {
            return (
                String::new(),
                Err(JarvisError::ContextBuilding("Session changed".into())),
            );
        }
        let mut auth_changes = self.auth_state().subscribe();
        if !self.jarvis_session_is_current(owner) {
            return (
                String::new(),
                Err(JarvisError::ContextBuilding("Session changed".into())),
            );
        }
        let command =
            self.jarvis_execute_in_context(cmd, &context, Some(&owner.user_id), Some(owner));
        tokio::pin!(command);
        loop {
            tokio::select! {
                result = &mut command => return result,
                _ = auth_changes.recv() => {
                    if !self.jarvis_session_is_current(owner) {
                        return (String::new(), Err(JarvisError::ContextBuilding("Session changed".into())));
                    }
                }
            }
        }
    }

    fn jarvis_session_is_current(&self, owner: JarvisSessionOwner) -> bool {
        let auth = self.auth_state().get();
        auth.current_user_id == Some(owner.user_id) && auth.session_revision == owner.revision
    }

    async fn jarvis_execute_in_context(
        &self,
        cmd: crate::services::jarvis::JarvisCommand,
        context: &crate::domain::jarvis::JarvisContext,
        author_id: Option<&crate::domain::ids::UserId>,
        owner: Option<JarvisSessionOwner>,
    ) -> (String, Result<String, crate::services::jarvis::JarvisError>) {
        use crate::services::jarvis::JarvisCommand as JC;

        use crate::domain::jarvis::ContextType;
        let channel_label = match context.context_type {
            ContextType::Channel => Some(context.context_id.clone()),
            _ => None,
        };

        let (query_display, state_message, result) = match cmd {
            JC::Ask(query) => {
                let r = self.jarvis_ask(context, &query, author_id).await;
                (format!("/ask {}", query), query, r)
            }
            JC::Summarize => {
                let r = self.jarvis_summarize_conversation(context).await;
                ("/summarize".to_string(), "/summarize".to_string(), r)
            }
            JC::Knowledge(entity) => {
                let label = format!("/knowledge {}", entity);
                let query = format!("What do you know about {}?", entity);
                let r = self.jarvis_ask(context, &query, author_id).await;
                (label.clone(), label, r)
            }
            JC::Remember(fact) => {
                let label = format!("/remember {}", fact);
                let query = format!("Please remember this fact: {}", fact);
                let r = self.jarvis_ask(context, &query, author_id).await;
                (label.clone(), label, r)
            }
        };

        if owner.is_some_and(|owner| !self.jarvis_session_is_current(owner)) {
            return (
                query_display,
                Err(crate::services::jarvis::JarvisError::ContextBuilding(
                    "Session changed".into(),
                )),
            );
        }
        self.jarvis_state()
            .add_user_message(&state_message, channel_label.as_deref());
        match &result {
            Ok(response) => {
                self.jarvis_state().add_assistant_message(response);
            }
            Err(e) => {
                self.jarvis_state()
                    .add_assistant_message(&format!("Error: {}", e));
            }
        }

        (query_display, result)
    }

    pub async fn jarvis_summarize_conversation(
        &self,
        context: &crate::domain::jarvis::JarvisContext,
    ) -> Result<String, crate::services::jarvis::JarvisError> {
        self.check_ai_allowed_in(context)?;
        let owner = self.auth_state().get().current_user_id.ok_or_else(||
            crate::services::jarvis::JarvisError::NotPermitted("Sign in before using remote AI".into()))?;
        self.jarvis_ask(context, "Summarize this remote conversation, highlighting main topics, decisions and next steps.", Some(&owner)).await
    }
}

/// "40 messages · 1 summary (40 folded) · 5 facts (3 current) · 12 entities".
/// Facts are the current user's (history included, as the browser lists
/// them); entities are the whole graph, not the browser's top-N.
fn memory_stats_label(
    stats: &crate::services::jarvis::MemoryStats,
    facts: usize,
    current_facts: usize,
) -> String {
    let plural = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = vec![plural(stats.conversation_count, "message", "messages")];
    if stats.summary_count > 0 {
        parts.push(format!(
            "{} ({} folded)",
            plural(stats.summary_count, "summary", "summaries"),
            stats.total_messages_summarized
        ));
    }
    parts.push(format!("{facts} facts ({current_facts} current)"));
    parts.push(plural(stats.entity_count, "entity", "entities"));
    parts.join(" · ")
}

/// `(statement, leaves_device)` for the AI channel on `silo`.
fn jarvis_privacy_for(silo: crate::ai_policy::policy::Silo, _engine: &str) -> (String, bool) {
    use crate::ai_policy::policy::Silo;
    match silo {
        Silo::Local => (
            "Remote AI is off or not configured; no prompt is sent".to_string(),
            false,
        ),
        Silo::Cloud => (
            "Cloud AI · messages are sent to your cloud AI provider".to_string(),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JarvisRow, jarvis_channel_reply_text, jarvis_privacy_for, memory_stats_label,
        parse_jarvis_mention,
    };
    use crate::domain::jarvis::{JarvisContext, JarvisConversation};

    #[test]
    fn jarvis_rows_carry_sender_content_and_state() {
        let t = JarvisRow::thinking();
        assert!(t.thinking && !t.is_own && t.id.starts_with("jarvis-thinking-"));
        assert_eq!(t.sender_name, "Jarvis");

        let r = JarvisRow::inline_reply("why?", "because");
        assert_eq!(r.content, "**Q:** why?\n\nbecause");
        assert!(!r.thinking && !r.is_own);

        let own = JarvisRow::own("Ana", "hi");
        assert!(own.is_own && own.sender_name == "Ana" && own.content == "hi");

        let ctx = JarvisContext::general();
        let mut user = JarvisConversation::user_message(&ctx, "q".into());
        user.timestamp = 7;
        let row = JarvisRow::from_history(&user, "Ana");
        assert!(row.is_own && row.sender_name == "Ana" && row.timestamp_ns == 7);
        let bot = JarvisRow::from_history(
            &JarvisConversation::assistant_message(&ctx, "a".into()),
            "Ana",
        );
        assert!(!bot.is_own && bot.sender_name == "Jarvis");
    }

    #[test]
    fn channel_reply_text_points_at_settings_on_error() {
        assert_eq!(jarvis_channel_reply_text(&Ok("yo".into())), "yo");
        let err =
            jarvis_channel_reply_text(&Err(crate::services::jarvis::JarvisError::NotEnabledHere));
        assert!(err.starts_with("Error: ") && err.contains("Settings → Remote AI"));
    }
    use crate::services::jarvis::MemoryStats;

    #[test]
    fn memory_stats_label_reports_real_totals() {
        let stats = MemoryStats {
            conversation_count: 20,
            summary_count: 1,
            total_messages_summarized: 40,
            fact_count: 3,
            entity_count: 37,
        };
        assert_eq!(
            memory_stats_label(&stats, 5, 3),
            "20 messages · 1 summary (40 folded) · 5 facts (3 current) · 37 entities"
        );
        let empty = MemoryStats {
            conversation_count: 1,
            summary_count: 0,
            total_messages_summarized: 0,
            fact_count: 0,
            entity_count: 1,
        };
        assert_eq!(
            memory_stats_label(&empty, 0, 0),
            "1 message · 0 facts (0 current) · 1 entity"
        );
    }
    use crate::ai_policy::policy::Silo;

    /// Only a local engine may say the messages stay here; cloud says where
    /// they go and flags that they leave the device.
    #[test]
    fn jarvis_privacy_line_follows_the_silo() {
        let (local, local_leaves) = jarvis_privacy_for(Silo::Local, "llama");
        assert!(local.contains("no prompt is sent") && !local.contains("Local AI"));
        assert!(!local_leaves);
        let (cloud, cloud_leaves) = jarvis_privacy_for(Silo::Cloud, "llama");
        assert!(cloud.contains("cloud AI provider") && !cloud.contains("never leaves"));
        assert!(cloud_leaves);
    }

    #[test]
    fn jarvis_mention_is_a_whole_word() {
        assert_eq!(
            parse_jarvis_mention("@Jarvis what time is it?"),
            Some("what time is it?")
        );
        assert_eq!(
            parse_jarvis_mention("Hey @jarvis, help me"),
            Some("help me")
        );
        assert_eq!(
            parse_jarvis_mention("  @JARVIS:  summarize this  "),
            Some("summarize this")
        );
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

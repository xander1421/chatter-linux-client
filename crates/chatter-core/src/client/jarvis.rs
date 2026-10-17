//! jarvis façade — thin delegates to services.

use super::ChatterClient;
use crate::domain::channel::{Channel, ChannelType};

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
/// are local-only, so these never come from the message store; the UI only
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
            "Error: {}\n\nCheck Settings → AI Assistant: is a local provider (Ollama, LM Studio, …) running and a model selected?",
            e
        ),
    }
}

pub struct LocalAiModelChoices {
    pub active_models: Vec<String>,
    pub providers: Vec<LocalAiProviderChoice>,
    pub catalog: Vec<LocalAiCatalogChoice>,
}

pub struct LocalAiProviderChoice {
    pub kind: String,
    pub display_name: String,
    pub detail: String,
    pub is_active: bool,
}

pub struct LocalAiCatalogChoice {
    pub name: String,
    pub tag: String,
    pub detail: String,
    pub fit: &'static str,
    pub fit_label: &'static str,
    pub installed: bool,
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
        let available = self.http_local_ai().cloud_settings().is_available();
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

    /// Resolve provider and catalog choices for the local AI settings view.
    pub fn local_ai_model_choices(
        &self,
        providers: &[crate::local_ai::providers::DetectedProvider],
        active_provider: &str,
        profile: &crate::local_ai::hardware::HardwareProfile,
    ) -> LocalAiModelChoices {
        use crate::local_ai::catalog::{CATALOG, ModelKind};
        use crate::local_ai::hardware::ModelFit;
        use crate::local_ai::providers::ProviderKind;
        let active_models = providers.iter()
            .find(|p| p.kind.as_str() == active_provider)
            .map(|p| p.models.iter().map(|m| m.name.clone())
                .filter(|name| {
                    let n = name.to_lowercase();
                    !n.contains("embed") && !n.contains("minilm") && !n.contains("bge")
                }).collect()).unwrap_or_default();
        let ollama_models: Vec<_> = providers.iter()
            .find(|p| p.kind == ProviderKind::Ollama)
            .map(|p| p.models.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default();
        let providers = providers.iter().map(|p| LocalAiProviderChoice {
            kind: p.kind.as_str().into(), display_name: p.kind.display_name().into(),
            detail: if p.kind == ProviderKind::MistralRs {
                "in-process GPU engine (loads models from HuggingFace)".into()
            } else { format!("{} models · {}", p.models.len(), p.url) },
            is_active: p.kind.as_str() == active_provider,
        }).collect();
        let catalog = CATALOG.iter().map(|m| {
            let installed = ollama_models.iter().any(|name: &String|
                name == m.ollama_tag || name.starts_with(&format!("{}:", m.ollama_tag)));
            let (fit, fit_label) = match m.fit(profile) {
                ModelFit::Gpu => ("gpu", "Fits GPU"),
                ModelFit::CpuSlow => ("cpu", "CPU only (slow)"),
                ModelFit::TooBig => ("big", "Too big for this machine"),
            };
            let kind_label = match m.kind {
                ModelKind::Chat => "", ModelKind::Embedding => " · embeddings",
            };
            LocalAiCatalogChoice {
                name: m.name.into(), tag: m.ollama_tag.into(),
                detail: format!("{}B params · {:.1} GB{}", m.params_b, m.size_gb, kind_label),
                fit, fit_label, installed,
            }
        }).collect();
        LocalAiModelChoices { active_models, providers, catalog }
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
        self.services
            .jarvis_engine
            .set(crate::services::jarvis::JarvisEngine::from_settings(
                &settings,
                self.local_ai().actor(),
            ));
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
            CloudMode::DirectAnthropic => JarvisEngine::from_cloud_settings(&cloud),
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
            settings
                .request_timeout
                .max(std::time::Duration::from_secs(3600)),
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
        self.jarvis()
            .clear_memory(&crate::domain::jarvis::JarvisContext::general())
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
        // The turn just grew this context's history; once it passes the
        // builder's window, fold the oldest turns into a summary. Off the
        // reply's path: it is one more LLM call.
        let jarvis = self.jarvis().clone();
        let ctx = context.clone();
        self.runtime_handle().spawn(async move {
            match jarvis.compress_memory(&ctx).await {
                Ok(true) => tracing::info!(context_id = %ctx.context_id, "Jarvis history compressed"),
                Ok(false) => {}
                Err(e) => tracing::warn!(context_id = %ctx.context_id, "Jarvis history compression failed: {e}"),
            }
        });
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
        use crate::services::jarvis::JarvisCommand as JC;

        use crate::domain::jarvis::ContextType;
        let channel_label = match context.context_type {
            ContextType::Channel => Some(context.context_id.clone()),
            _ => None,
        };

        let (query_display, result) = match cmd {
            JC::Ask(query) => {
                self.jarvis_state()
                    .add_user_message(&query, channel_label.as_deref());
                let r = self.jarvis_ask(context, &query, author_id).await;
                (format!("/ask {}", query), r)
            }
            JC::Summarize => {
                self.jarvis_state()
                    .add_user_message("/summarize", channel_label.as_deref());
                let r = self.jarvis_summarize_conversation(context).await;
                ("/summarize".to_string(), r)
            }
            JC::Knowledge(entity) => {
                let label = format!("/knowledge {}", entity);
                self.jarvis_state()
                    .add_user_message(&label, channel_label.as_deref());
                let query = format!("What do you know about {}?", entity);
                let r = self.jarvis_ask(context, &query, author_id).await;
                (label, r)
            }
            JC::Remember(fact) => {
                let label = format!("/remember {}", fact);
                self.jarvis_state()
                    .add_user_message(&label, channel_label.as_deref());
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
        self.jarvis().summarize_conversation(context).await
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
fn jarvis_privacy_for(silo: crate::ai_policy::policy::Silo, engine: &str) -> (String, bool) {
    use crate::ai_policy::policy::Silo;
    match silo {
        Silo::Local => (
            format!("Local AI · {engine} · never leaves this device"),
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
        assert!(err.starts_with("Error: ") && err.contains("Settings → AI Assistant"));
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
        assert!(local.contains("never leaves this device") && local.contains("llama"));
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

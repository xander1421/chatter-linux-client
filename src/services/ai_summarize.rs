//! AI summarization service.
//!
//! Provides conversation and thread summarization capabilities using server-side
//! AI integrations. Validates prerequisites and prepares prompts for summarization.

use std::sync::Arc;

use crate::error::NetworkError;
use crate::network::grpc::integration_client::IntegrationClient;
use crate::proto::chat_v1::{Integration, IntegrationType, Message};
use crate::services::ai_service::AIService;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Summarization service errors.
#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// No AI integration configured for the channel.
    #[error("no AI integration configured for this channel")]
    NoIntegrationConfigured,

    /// Summarization failed.
    #[error("summarization failed: {0}")]
    SummarizationFailed(String),
}

/// AI summarization service.
///
/// Provides conversation summarization by validating that a channel has an AI
/// integration and preparing prompts for summarization requests.
///
/// # Summarization Flow
///
/// 1. UI/service calls `request_summary` to validate the channel has an AI integration
/// 2. If valid, UI sends a message with `targets_ai_bot=true` containing the prompt
/// 3. AI integration receives it and responds via streaming events
/// 4. The response comes back as an AIBotContent message
///
/// # Example
///
/// ```ignore
/// let summarize_service = AISummarizeService::new(settings_repo);
/// let ai_service = AIService::new(settings_repo);
///
/// // Get the channel's AI integration (if any)
/// let integration = summarize_service.get_channel_ai_integration(&ai_service, &mut client, "channel-123").await?;
///
/// // Prepare a summarization prompt
/// let prompt = AISummarizeService::prepare_summarization_prompt(&messages, Some("Summarize the key decisions"));
///
/// // Validate prerequisites for summary request
/// summarize_service.request_summary(&ai_service, &mut client, "channel-123", &messages, None).await?;
/// ```
pub struct AISummarizeService {
    settings_repo: Arc<SettingsRepository>,
}

impl AISummarizeService {
    /// Create a new AISummarizeService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self { settings_repo }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<String, SummarizeError> {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(|_| SummarizeError::NotLoggedIn)?
            .ok_or(SummarizeError::NotLoggedIn)
    }

    /// Check if an integration type is an AI provider.
    ///
    /// AI providers are types 10-15 in the IntegrationType enum:
    /// - Claude (10)
    /// - OpenAI (11)
    /// - GoogleAi (12)
    /// - AzureOpenai (13)
    /// - Cohere (14)
    /// - Huggingface (15)
    pub fn is_ai_integration_type(integration_type: i32) -> bool {
        (10..=15).contains(&integration_type)
    }

    /// Get the first active AI integration for a channel.
    ///
    /// Queries the channel's integrations and returns the first one that is
    /// an AI provider type (Claude, OpenAI, etc.).
    ///
    /// # Arguments
    ///
    /// * `ai_service` - Reference to AIService for integration queries
    /// * `client` - Mutable reference to integration client
    /// * `channel_id` - Channel to query
    ///
    /// # Returns
    ///
    /// The first AI integration found, or None if no AI integration is configured.
    pub async fn get_channel_ai_integration(
        &self,
        ai_service: &AIService,
        client: &mut IntegrationClient,
        channel_id: &str,
    ) -> Result<Option<Integration>, SummarizeError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Get all channel integrations
        let integrations = ai_service
            .list_channel_integrations(client, channel_id)
            .await
            .map_err(|e| match e {
                crate::services::ai_service::AIError::NotLoggedIn => SummarizeError::NotLoggedIn,
                crate::services::ai_service::AIError::Network(n) => SummarizeError::Network(n),
                _ => SummarizeError::SummarizationFailed(e.to_string()),
            })?;

        // Find first AI integration
        let ai_integration = integrations
            .into_iter()
            .find(|i| Self::is_ai_integration_type(i.r#type));

        Ok(ai_integration)
    }

    /// Prepare a summarization prompt from messages.
    ///
    /// Builds a prompt suitable for AI summarization by combining an instruction
    /// with formatted message content.
    ///
    /// # Arguments
    ///
    /// * `messages` - Messages to summarize
    /// * `instruction` - Optional custom instruction. Defaults to
    ///                   "Summarize the following conversation concisely"
    ///
    /// # Returns
    ///
    /// A formatted prompt string ready to send to an AI integration.
    pub fn prepare_summarization_prompt(messages: &[Message], instruction: Option<&str>) -> String {
        let default_instruction = "Summarize the following conversation concisely";
        let instruction = instruction.unwrap_or(default_instruction);

        let mut prompt = format!("{}\n\n", instruction);

        for msg in messages {
            // Get author (use "Anonymous" for sealed sender)
            let author = if msg.author_id.is_empty() {
                "Anonymous".to_string()
            } else {
                msg.author_id.clone()
            };

            // Extract text content
            let content_text = if let Some(content) = &msg.content {
                extract_text_content(content)
            } else {
                "[No content]".to_string()
            };

            prompt.push_str(&format!("{}: {}\n", author, content_text));
        }

        prompt
    }

    /// Request a summary for channel messages.
    ///
    /// Validates that the channel has an AI integration configured and that
    /// messages are provided for summarization. This method validates
    /// prerequisites; the actual summarization is triggered by the caller
    /// sending a message with `targets_ai_bot=true`.
    ///
    /// # Arguments
    ///
    /// * `ai_service` - Reference to AIService for integration queries
    /// * `client` - Mutable reference to integration client
    /// * `channel_id` - Channel to summarize messages from
    /// * `messages` - Messages to summarize
    /// * `instruction` - Optional custom summarization instruction
    ///
    /// # Returns
    ///
    /// `Ok(())` if prerequisites are met (channel has AI integration and messages provided).
    /// The caller should then send a message with `targets_ai_bot=true` containing
    /// the prompt from `prepare_summarization_prompt`.
    pub async fn request_summary(
        &self,
        ai_service: &AIService,
        client: &mut IntegrationClient,
        channel_id: &str,
        messages: &[Message],
        instruction: Option<&str>,
    ) -> Result<(), SummarizeError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Validate messages provided
        if messages.is_empty() {
            return Err(SummarizeError::SummarizationFailed(
                "no messages to summarize".to_string(),
            ));
        }

        // Check channel has AI integration
        let ai_integration = self
            .get_channel_ai_integration(ai_service, client, channel_id)
            .await?;

        if ai_integration.is_none() {
            return Err(SummarizeError::NoIntegrationConfigured);
        }

        // Validate instruction if provided
        if let Some(inst) = instruction {
            if inst.trim().is_empty() {
                return Err(SummarizeError::SummarizationFailed(
                    "instruction cannot be empty".to_string(),
                ));
            }
        }

        // Prerequisites met - caller can now send the message with targets_ai_bot=true
        Ok(())
    }

    /// Get the AI provider name for a given integration type.
    ///
    /// Returns a human-readable name for the AI provider.
    pub fn provider_name(integration_type: i32) -> &'static str {
        match IntegrationType::try_from(integration_type) {
            Ok(IntegrationType::Claude) => "Claude",
            Ok(IntegrationType::Openai) => "OpenAI",
            Ok(IntegrationType::GoogleAi) => "Google AI",
            Ok(IntegrationType::AzureOpenai) => "Azure OpenAI",
            Ok(IntegrationType::Cohere) => "Cohere",
            Ok(IntegrationType::Huggingface) => "Hugging Face",
            _ => "Unknown",
        }
    }
}

/// Extract text content from a message Content.
fn extract_text_content(content: &crate::proto::chat_v1::message::Content) -> String {
    use crate::proto::chat_v1::message::content::Type;

    match &content.r#type {
        Some(Type::Text(text)) => text.clone(),
        Some(Type::Encrypted(_)) => "[Encrypted message]".to_string(),
        Some(Type::File(file_meta)) => format!("[File: {}]", file_meta.name),
        Some(Type::Reaction(reaction)) => format!("[Reaction: {}]", reaction.emoji),
        Some(Type::Mixed(mixed)) => {
            if mixed.text.is_empty() {
                format!("[{} file(s)]", mixed.files.len())
            } else {
                mixed.text.clone()
            }
        }
        Some(Type::AiBot(ai)) => ai.text.clone(),
        Some(Type::Call(call)) => format!("[Call: {}]", call.call_id),
        Some(Type::SignalEncrypted(_)) => "[Signal encrypted message]".to_string(),
        Some(Type::SealedSender(_)) => "[Sealed sender message]".to_string(),
        None => "[No content]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarize_error_display() {
        let err = SummarizeError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = SummarizeError::NoIntegrationConfigured;
        assert!(err.to_string().contains("no AI integration"));

        let err = SummarizeError::SummarizationFailed("test error".to_string());
        assert!(err.to_string().contains("summarization failed"));
        assert!(err.to_string().contains("test error"));

        let network_err = SummarizeError::Network(NetworkError::Timeout);
        assert!(network_err.to_string().contains("network error"));
    }

    #[test]
    fn test_summarize_error_from_network() {
        let network_err = NetworkError::Unauthenticated;
        let sum_err: SummarizeError = network_err.into();
        assert!(matches!(sum_err, SummarizeError::Network(_)));
    }

    #[test]
    fn test_is_ai_integration_type() {
        // AI types (10-15)
        assert!(AISummarizeService::is_ai_integration_type(10)); // Claude
        assert!(AISummarizeService::is_ai_integration_type(11)); // OpenAI
        assert!(AISummarizeService::is_ai_integration_type(12)); // GoogleAi
        assert!(AISummarizeService::is_ai_integration_type(13)); // AzureOpenai
        assert!(AISummarizeService::is_ai_integration_type(14)); // Cohere
        assert!(AISummarizeService::is_ai_integration_type(15)); // Huggingface

        // Non-AI types
        assert!(!AISummarizeService::is_ai_integration_type(0));  // Unspecified
        assert!(!AISummarizeService::is_ai_integration_type(1));  // Webhook
        assert!(!AISummarizeService::is_ai_integration_type(2));  // Slack
        assert!(!AISummarizeService::is_ai_integration_type(16)); // Out of range
        assert!(!AISummarizeService::is_ai_integration_type(100)); // Custom
    }

    #[test]
    fn test_prepare_summarization_prompt_default_instruction() {
        let messages = vec![
            create_test_message("user1", "Hello everyone"),
            create_test_message("user2", "Hi there!"),
            create_test_message("user1", "How's the project going?"),
        ];

        let prompt = AISummarizeService::prepare_summarization_prompt(&messages, None);

        assert!(prompt.starts_with("Summarize the following conversation concisely"));
        assert!(prompt.contains("user1: Hello everyone"));
        assert!(prompt.contains("user2: Hi there!"));
        assert!(prompt.contains("user1: How's the project going?"));
    }

    #[test]
    fn test_prepare_summarization_prompt_custom_instruction() {
        let messages = vec![
            create_test_message("user1", "We decided to use Rust"),
            create_test_message("user2", "Agreed, for safety"),
        ];

        let instruction = "List the key decisions made";
        let prompt = AISummarizeService::prepare_summarization_prompt(&messages, Some(instruction));

        assert!(prompt.starts_with("List the key decisions made"));
        assert!(prompt.contains("user1: We decided to use Rust"));
        assert!(prompt.contains("user2: Agreed, for safety"));
    }

    #[test]
    fn test_prepare_summarization_prompt_empty_messages() {
        let messages: Vec<Message> = vec![];
        let prompt = AISummarizeService::prepare_summarization_prompt(&messages, None);

        // Should just have the instruction
        assert!(prompt.starts_with("Summarize the following conversation concisely"));
        assert!(prompt.trim().ends_with("concisely"));
    }

    #[test]
    fn test_prepare_summarization_prompt_anonymous_author() {
        let mut msg = create_test_message("", "Sealed sender message");
        msg.author_id = String::new(); // Empty = sealed sender

        let messages = vec![msg];
        let prompt = AISummarizeService::prepare_summarization_prompt(&messages, None);

        assert!(prompt.contains("Anonymous: Sealed sender message"));
    }

    #[test]
    fn test_provider_name() {
        assert_eq!(AISummarizeService::provider_name(10), "Claude");
        assert_eq!(AISummarizeService::provider_name(11), "OpenAI");
        assert_eq!(AISummarizeService::provider_name(12), "Google AI");
        assert_eq!(AISummarizeService::provider_name(13), "Azure OpenAI");
        assert_eq!(AISummarizeService::provider_name(14), "Cohere");
        assert_eq!(AISummarizeService::provider_name(15), "Hugging Face");
        assert_eq!(AISummarizeService::provider_name(0), "Unknown");
        assert_eq!(AISummarizeService::provider_name(99), "Unknown");
    }

    #[test]
    fn test_ai_summarize_service_construction() {
        // Verify the AISummarizeService struct type signatures compile correctly.
        fn _verify_types(settings_repo: Arc<SettingsRepository>) {
            let _service = AISummarizeService::new(settings_repo);
        }
    }

    // Helper function to create test messages
    fn create_test_message(author_id: &str, text: &str) -> Message {
        use crate::proto::chat_v1::message::{content::Type, Content};

        Message {
            id: "msg-1".to_string(),
            channel_id: "channel-1".to_string(),
            author_id: author_id.to_string(),
            parent_id: String::new(),
            content: Some(Content {
                r#type: Some(Type::Text(text.to_string())),
            }),
            timestamp: 1234567890000,
            signature: vec![],
            ephemeral: false,
            ephemeral_ttl: None,
            pinned: false,
            reply_count: 0,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            crypto_status: None,
            updated_at: 0,
            edited: false,
            edited_at: 0,
            mentioned_integrations: vec![],
            targets_ai_bot: false,
        }
    }
}

//! Jarvis error types.

use thiserror::Error;

/// Errors that can occur in Jarvis operations.
#[derive(Debug, Error)]
pub enum JarvisError {
    /// Storage error.
    #[error("storage error: {0}")]
    Storage(#[from] crate::error::StorageError),

    /// Local AI error.
    #[error("local AI error: {0}")]
    #[cfg(feature = "local-ai")]
    LocalAI(#[from] crate::services::ai::LocalAIError),

    /// HTTP provider backend error.
    #[error("provider error: {0}")]
    Backend(#[from] crate::ai_protocol::BackendError),

    /// Entity extraction failed.
    #[error("entity extraction failed: {0}")]
    EntityExtraction(String),

    /// Context building failed.
    #[error("context building failed: {0}")]
    ContextBuilding(String),

    /// No model loaded.
    #[error("Remote AI is not configured")]
    NoModel,

    /// Jarvis is disabled.
    #[error("Jarvis is disabled")]
    Disabled,

    /// AI is switched off for this conversation.
    ///
    /// Distinct from `NotPermitted`: that means policy forbids the *engine*
    /// (cloud in a DM, anything over mesh); this means the user simply has not
    /// enabled AI in this channel yet, which is recoverable by toggling it on.
    #[error("AI is not enabled in this conversation")]
    NotEnabledHere,

    /// The conversation's AI policy forbids this engine.
    ///
    /// Raised when a cloud provider is asked to run on a DM, or any provider
    /// on mesh-delivered traffic. See `local_ai::policy`.
    #[error("AI not permitted here: {0}")]
    NotPermitted(String),
}

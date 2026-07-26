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
    LocalAI(#[from] crate::services::ai::LocalAIError),

    /// HTTP provider backend error.
    #[error("provider error: {0}")]
    Backend(#[from] crate::local_ai::backend::BackendError),

    /// Entity extraction failed.
    #[error("entity extraction failed: {0}")]
    EntityExtraction(String),

    /// Context building failed.
    #[error("context building failed: {0}")]
    ContextBuilding(String),

    /// No model loaded.
    #[error("no model loaded - call load_model first")]
    NoModel,

    /// Jarvis is disabled.
    #[error("Jarvis is disabled")]
    Disabled,

    /// The conversation's AI policy forbids this engine.
    ///
    /// Raised when a cloud provider is asked to run on a DM, or any provider
    /// on mesh-delivered traffic. See `local_ai::policy`.
    #[error("AI not permitted here: {0}")]
    NotPermitted(String),
}

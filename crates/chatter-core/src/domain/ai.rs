//! AI integration domain entity.
//!
//! Represents AI integrations and local AI backends.

/// AI integration configuration.
pub struct AIIntegration;

/// AI provider information.
pub struct Provider;

/// Local AI backend types.
pub enum LocalAIBackend {
    /// Ollama backend.
    Ollama,
    /// vLLM backend.
    VLLM,
    /// MLC-LLM backend.
    MLCLLM,
}

/// AI message for chat completions.
pub struct AIMessage;

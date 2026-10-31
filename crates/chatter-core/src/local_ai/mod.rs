//! Local AI infrastructure.
//!
//! HTTP backends (Ollama native; LM Studio / llama.cpp / vLLM via the
//! OpenAI-compatible API), provider detection, hardware profiling, model
//! catalog, embeddings and semantic-search plumbing.

pub mod backend;
#[cfg(feature = "local-ai")]
pub mod catalog;
#[cfg(feature = "local-ai")]
pub mod embeddings;
#[cfg(feature = "local-ai")]
pub mod hardware;
#[cfg(feature = "local-ai")]
pub mod indexer;
#[cfg(feature = "local-ai")]
pub mod model_manager;
#[cfg(feature = "local-ai")]
pub mod ollama;
#[cfg(feature = "local-ai")]
pub mod openai_compat;
#[cfg(feature = "local-ai")]
pub mod providers;
#[cfg(feature = "local-ai")]
pub mod settings;
#[cfg(feature = "local-ai")]
pub mod vector_store;

//! Local AI infrastructure.
//!
//! HTTP backends (Ollama native; LM Studio / llama.cpp / vLLM via the
//! OpenAI-compatible API), provider detection, hardware profiling, model
//! catalog, embeddings and semantic-search plumbing.

pub mod anthropic;
pub mod backend;
pub mod catalog;
pub mod channel_ai;
pub mod hardware;
pub mod ollama;
pub mod openai_compat;
pub mod policy;
pub mod providers;
pub mod model_manager;
pub mod embeddings;
pub mod vector_store;
pub mod indexer;
pub mod settings;

//! Local AI infrastructure.
//!
//! HTTP backends (Ollama native; LM Studio / llama.cpp / vLLM via the
//! OpenAI-compatible API), provider detection, hardware profiling, model
//! catalog, embeddings and semantic-search plumbing.

pub mod backend;
pub mod catalog;
pub mod embeddings;
pub mod hardware;
pub mod indexer;
pub mod model_manager;
pub mod ollama;
pub mod openai_compat;
pub mod providers;
pub mod settings;
pub mod vector_store;

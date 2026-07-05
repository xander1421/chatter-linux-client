//! Local AI infrastructure.
//!
//! Support for Ollama, vLLM, and MLC-LLM backends.

pub mod backend;
pub mod ollama;
pub mod model_manager;
pub mod embeddings;
pub mod vector_store;
pub mod indexer;
pub mod settings;

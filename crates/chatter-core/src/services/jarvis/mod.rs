//! Jarvis AI assistant service.
//!
//! Provides a persistent AI assistant with:
//! - **Memory**: Conversation history, facts, and summaries
//! - **Knowledge Graph**: Entity tracking and relationships
//! - **Context Building**: RAG-style retrieval for enriched responses
//! - **Background Learning**: Passive observation of messages
//!
//! # Architecture
//!
//! ```text
//! JarvisService (orchestrator)
//!     ├── MemoryManager (conversation history, facts, summaries)
//!     ├── EntityExtractor (NLP via LocalAI)
//!     ├── ContextBuilder (RAG-style retrieval)
//!     ├── JarvisMemoryRepository (SQLite)
//!     └── JarvisGraphRepository (SQLite with CTEs)
//! ```
//!
//! # Usage
//!
//! ```ignore
//! // Ask Jarvis a question
//! let context = JarvisContext::channel(channel_id);
//! let response = jarvis.ask(&context, "What project is Alice working on?", Some(&user_id)).await?;
//!
//! // Observe a message for learning
//! jarvis.observe_message(&context, "I'm working on the Chatter project", Some(&user_id), None).await?;
//!
//! // Get entity knowledge
//! let knowledge = jarvis.get_entity_knowledge("Alice")?;
//! ```

pub mod background;
pub mod commands;
pub mod context_builder;
pub mod engine;
pub mod entity_extractor;
pub mod fact_extractor;
pub mod error;
pub mod memory_manager;
pub mod service;

pub use background::{JarvisBackground, JarvisBackgroundBuilder, JarvisBackgroundHandles};
pub use commands::{JarvisCommand, parse_slash_command};
pub use context_builder::{BuiltContext, ContextBuilder};
pub use engine::{JarvisEngine, SharedJarvisEngine};
pub use entity_extractor::{EntityExtractor, ExtractedEntity};
pub use fact_extractor::{ExtractedFact, FactExtractor};
pub use error::JarvisError;
pub use memory_manager::{CompressionConfig, MemoryManager, MemoryStats};
pub use service::JarvisService;

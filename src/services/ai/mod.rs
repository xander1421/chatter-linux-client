//! Local AI services module.
//!
//! Provides GPU-accelerated local LLM inference via an actor pattern.

mod local_actor;
mod local_service;

pub use local_actor::{
    LocalAIActorHandle, LocalAICommand, LocalAIError, ModelInfo, SampleParams,
};
pub use local_service::LocalAIService;

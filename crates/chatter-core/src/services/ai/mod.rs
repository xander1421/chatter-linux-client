//! Shared generation parameters; legacy local inference is explicitly opt-in.
mod sample_params;
pub use sample_params::SampleParams;
#[cfg(feature = "local-ai")]
mod local_actor;
#[cfg(feature = "local-ai")]
mod local_service;
#[cfg(feature = "local-ai")]
pub use local_actor::{LocalAIActorHandle, LocalAICommand, LocalAIError, ModelInfo};
#[cfg(feature = "local-ai")]
pub use local_service::LocalAIService;

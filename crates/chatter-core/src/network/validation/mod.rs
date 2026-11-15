//! Message validation layer.
//!
//! Admission control and validation for incoming messages.

pub mod deduplicator;
pub mod orderer;
pub mod validator;

pub use deduplicator::MessageDeduplicator;
pub use validator::{
    MessageValidationPipeline, PipelineResult,
};

//! Message validation layer.
//!
//! Admission control and validation for incoming messages.

pub mod deduplicator;
pub mod orderer;
pub mod protovalidate;
pub mod validator;

pub use deduplicator::{DeduplicatorStats, MessageDeduplicator};
pub use orderer::{MessageOrderer, OrdererStats, OrderingResult};
pub use validator::{
    ContentValidator, MessageValidationPipeline, PipelineResult, PipelineStats, ValidationError,
    ValidationLimits,
};

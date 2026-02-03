//! Message validation layer.
//!
//! Admission control and validation for incoming messages.

pub mod deduplicator;
pub mod orderer;
pub mod validator;

pub use deduplicator::{DeduplicatorStats, MessageDeduplicator};
pub use validator::{ContentValidator, ValidationError, ValidationLimits};

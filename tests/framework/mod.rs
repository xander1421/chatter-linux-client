//! Test framework for Chatter Linux Client.
//!
//! Provides a comprehensive testing infrastructure with:
//! - Auto-generated mocks via mockall
//! - Builder patterns for test data
//! - Test harnesses for different scenarios
//! - Reusable fixtures and utilities
//!
//! # Test Types
//!
//! - **Unit tests**: Fast, isolated, no external dependencies
//! - **Integration tests**: Require running server
//! - **E2E tests**: Full user journey scenarios
//! - **Functional tests**: Feature-specific tests
//!
//! # Usage
//!
//! ```rust
//! use crate::framework::prelude::*;
//!
//! #[tokio::test]
//! async fn test_something() {
//!     let harness = TestHarness::unit().await;
//!     let user = UserBuilder::new("alice").build();
//!     // ...
//! }
//! ```

pub mod prelude;
pub mod macros;
pub mod mocks;
pub mod builders;
pub mod harness;
pub mod fixtures;
pub mod traits;

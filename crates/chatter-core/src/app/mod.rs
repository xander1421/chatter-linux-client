//! Application orchestration.
//!
//! Top-level application initialization and management.
//!
//! # Three-Tier Architecture
//!
//! The application context is composed of three layers:
//!
//! 1. **InfrastructureContainer**: Low-level dependencies (storage, crypto, network)
//! 2. **ServiceContainer**: Pre-built services (business logic)
//! 3. **CoreServices**: Services + state stores + event bus (consumed by `ChatterClient`)
//!
//! External code interacts via `ChatterClient`, not `CoreServices` directly.

pub mod context;
pub mod core_services;
pub mod infrastructure;
mod server_endpoint;
pub mod services;

pub use context::{AppContext, ContextError};
pub use core_services::CoreServices;
pub use infrastructure::InfrastructureContainer;

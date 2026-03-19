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
//! 3. **UIContext**: Services + state stores (UI-facing API)
//!
//! UI code should primarily interact with `UIContext` via `AppContext::ui_context()`.

pub mod bootstrap;
pub mod config;
pub mod context;
pub mod infrastructure;
pub mod services;
pub mod shutdown;
pub mod ui_context;

pub use context::{AppContext, ContextError};
pub use infrastructure::InfrastructureContainer;
pub use ui_context::UIContext;

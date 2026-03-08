//! Integration tests for UI callbacks.
//!
//! Tests that UI callbacks correctly trigger server operations
//! and update application state.
//!
//! Note: These tests verify callback wiring but cannot fully test
//! the Slint UI without a display. They focus on state transitions.

mod auth_tests;
mod chat_tests;
mod composer_tests;
mod dm_tests;
mod header_tests;
mod modal_tests;
mod navigation_tests;
mod settings_tests;
mod sidebar_tests;
mod state_tests;

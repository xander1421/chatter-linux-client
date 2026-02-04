//! Ephemeral Mailbox System (NDSS '21).
//!
//! Privacy-preserving message routing to prevent Statistical Disclosure Attacks.

pub mod blind_signature;
pub mod client;
pub mod content;
pub mod cover_traffic;
pub mod dm;
pub mod group;
pub mod key_rolling;
pub mod poller;
pub mod provider;
pub mod token_manager;

// Re-export key types for easier access
pub use blind_signature::{
    blind_key, unblind, verify, BlindingOperation, BlindingSecret, MessageRandomizer,
    ServerPublicKey, UnblindedToken,
};
pub use token_manager::{
    MailboxToken, QuotaStatus, TokenManager, DEFAULT_TOKEN_REQUEST_COUNT, MIN_TOKEN_THRESHOLD,
};

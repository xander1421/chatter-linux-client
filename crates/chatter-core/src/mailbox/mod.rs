//! Ephemeral Mailbox System (NDSS '21).
//!
//! Privacy-preserving message routing that resists Statistical Disclosure
//! Attacks (partial — see the cover-traffic note below).
//!
//! This module implements the protocol from "Improving Signal's Sealed Sender"
//! (NDSS 2021), using:
//!
//! - **Ephemeral Mailboxes**: Each conversation uses a unique, temporary mailbox
//! - **Blind Signatures**: Anonymous mailbox registration (server can't link user to mailbox)
//! - **Two-Way Anonymity**: After bootstrap, both parties communicate via ephemeral mailboxes
//! - **Key Rolling**: Periodic mailbox rotation for forward secrecy
//!
//! ## Traffic Analysis Resistance
//!
//! Additional privacy features to prevent traffic analysis:
//!
//! - **Content Padding** ([`content`]): Log-bucketing to hide message sizes
//! - **Timing Obfuscation** ([`timing`]): Random delays before operations
//! - **Dummy Polling** ([`poller`]): Polls to decoy mailboxes hide access patterns
//!
//! Note: rate/volume cover traffic (fake messages at random intervals) is NOT
//! implemented — it belongs with the broader anonymity-egress work (unlinkable
//! network path + traffic analysis) rather than as an inert module.

pub mod anon_dm;
pub mod blind_signature;
pub mod content;
pub mod dm;
pub mod group;
pub mod key_rolling;
pub mod timing;
pub mod token_manager;

// Re-export key types for easier access
pub use blind_signature::{
    BlindingOperation, BlindingSecret, MessageRandomizer, ServerPublicKey, UnblindedToken,
    blind_key, unblind, verify,
};
pub use token_manager::{
    // Call room tokens (integrated into TokenManager)
    CallRoomToken,
    DEFAULT_CALL_ROOM_TOKEN_REQUEST_COUNT,
    DEFAULT_TOKEN_REQUEST_COUNT,
    MIN_CALL_ROOM_TOKEN_THRESHOLD,
    MIN_TOKEN_THRESHOLD,
    // Mailbox tokens
    MailboxToken,
    TokenManager,
};

// Re-export DM types
pub use dm::{
    BootstrapResponse, BootstrapResult, DmBootstrap, DmConversation, DmConversationManager,
    DmMailbox, PollResult, ReceivedMessage, TwoWayAnonymity,
};

// Re-export group types
pub use group::{
    FanoutResult, GroupFanout, GroupFanoutResult, GroupMailbox, GroupMailboxManager,
    GroupMailboxState, GroupPollResult, MemberMailbox,
};

// Re-export content encoding types
pub use content::{ContentEncoder, MAX_CONTENT_SIZE, MIN_PADDED_SIZE};

// Re-export key rolling types
pub use key_rolling::{
    DEFAULT_CHECK_INTERVAL_SECS, DEFAULT_ROTATION_THRESHOLD_HOURS, KeyRollingService,
    RotationNotification, RotationResult, RotationTarget,
};

// Re-export timing obfuscation types
pub use timing::{
    DEFAULT_JITTER_PERCENT, DEFAULT_MAX_DELAY_MS, DEFAULT_MIN_DELAY_MS, TimingConfig,
    TimingObfuscation,
};

// Re-export anonymous DM polling types
pub use anon_dm::{AnonDmPollResult, AnonDmPoller};

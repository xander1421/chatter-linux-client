//! Ephemeral Mailbox System (NDSS '21).
//!
//! Privacy-preserving message routing to prevent Statistical Disclosure Attacks.
//!
//! This module implements the protocol from "Improving Signal's Sealed Sender"
//! (NDSS 2021) which prevents the Statistical Disclosure Attack by using:
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
//! - **Cover Traffic** ([`cover_traffic`]): Fake messages at random intervals
//! - **Dummy Polling** ([`poller`]): Polls to decoy mailboxes hide access patterns

pub mod anon_dm;
pub mod blind_signature;
pub mod content;
pub mod cover_traffic;
pub mod dm;
pub mod group;
pub mod key_rolling;
pub mod poller;
pub mod timing;
pub mod token_manager;

// Re-export key types for easier access
pub use blind_signature::{
    blind_key, unblind, verify, BlindingOperation, BlindingSecret, MessageRandomizer,
    ServerPublicKey, UnblindedToken,
};
pub use token_manager::{
    // Mailbox tokens
    MailboxToken, QuotaStatus, TokenManager, DEFAULT_TOKEN_REQUEST_COUNT, MIN_TOKEN_THRESHOLD,
    // Call room tokens (integrated into TokenManager)
    CallRoomToken, CallRoomQuotaStatus,
    DEFAULT_CALL_ROOM_TOKEN_REQUEST_COUNT, MIN_CALL_ROOM_TOKEN_THRESHOLD,
};

// Re-export DM types
pub use dm::{
    BootstrapResponse, BootstrapResult, DmBootstrap, DmConversation, DmConversationManager,
    DmMailbox, PollResult, ReceivedMessage, SendResult, TwoWayAnonymity,
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
    KeyRollingService, RotationNotification, RotationResult, RotationTarget,
    DEFAULT_CHECK_INTERVAL_SECS, DEFAULT_OVERLAP_SECS, DEFAULT_ROTATION_THRESHOLD_HOURS,
};

// Re-export cover traffic types
pub use cover_traffic::{
    CoverTrafficConfig, CoverTrafficGenerator, CoverTrafficMessage, CoverTrafficStats,
    DEFAULT_BASE_INTERVAL_SECS, MAX_INTERVAL_SECS, MIN_INTERVAL_SECS,
};

// Re-export timing obfuscation types
pub use timing::{
    TimingConfig, TimingObfuscation, DEFAULT_JITTER_PERCENT, DEFAULT_MAX_DELAY_MS,
    DEFAULT_MIN_DELAY_MS,
};

// Re-export poller types
pub use poller::{
    MailboxPoller, PollerConfig, PollerStats, UnifiedPollResult,
    DEFAULT_DUMMY_POLL_PROBABILITY, DEFAULT_POLL_INTERVAL_SECS,
};

// Re-export anonymous DM polling types
pub use anon_dm::{AnonDmPollResult, AnonDmPoller};

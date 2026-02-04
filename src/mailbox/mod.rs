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

//! Forward service.
//!
//! Forward messages to channels, DMs, and AI bots.

/// Forward service.
pub struct ForwardService;

/// Forward target options.
pub enum ForwardTarget {
    /// Forward to channel.
    Channel,
    /// Forward to DM.
    DirectMessage,
    /// Forward to AI bot.
    AIBot,
    /// Save to personal vault.
    SavedMessages,
}

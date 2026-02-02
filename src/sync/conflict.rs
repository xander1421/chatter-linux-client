//! Conflict resolution.
//!
//! Conflict resolution strategies for sync conflicts.

/// Conflict resolver.
pub struct ConflictResolver;

/// Conflict resolution strategy.
pub enum ConflictStrategy {
    /// Last write wins.
    LastWriteWins,
    /// First write wins.
    FirstWriteWins,
    /// Merge changes.
    Merge,
}

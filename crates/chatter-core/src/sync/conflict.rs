//! Conflict resolution.
//!
//! Simple last-write-wins conflict resolution for sync operations.
//! We don't use vector clocks — server timestamps are authoritative.

/// Conflict resolution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictStrategy {
    /// Server version always wins (default for sync).
    ServerWins,
    /// Local version wins (for unsent drafts).
    LocalWins,
    /// Use the newer timestamp.
    NewerWins,
}

impl Default for ConflictStrategy {
    fn default() -> Self {
        Self::ServerWins
    }
}

/// Resolve a conflict between local and server timestamps.
///
/// Returns `true` if the server version should be used.
pub fn should_use_server_version(
    local_updated_at: i64,
    server_updated_at: i64,
    strategy: ConflictStrategy,
) -> bool {
    match strategy {
        ConflictStrategy::ServerWins => true,
        ConflictStrategy::LocalWins => false,
        ConflictStrategy::NewerWins => server_updated_at >= local_updated_at,
    }
}

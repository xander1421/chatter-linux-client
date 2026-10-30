use std::sync::Arc;

use crate::storage::Database;

mod devices;
mod lifecycle;
mod load;
mod mailboxes;
mod pending;

/// Manager for DM conversation state persistence.
///
/// Handles database operations for storing and retrieving DM mailbox state.
pub struct DmConversationManager {
    /// Database connection for state storage.
    db: Arc<Database>,
}

impl DmConversationManager {
    /// Create a new DmConversationManager.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
}

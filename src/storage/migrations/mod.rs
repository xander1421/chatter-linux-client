//! Database migrations.
//!
//! Migration infrastructure for schema evolution with version tracking.

pub mod v001_initial;
pub mod v002_users;
pub mod v003_channels;
pub mod v004_messages;
pub mod v005_attachments;
pub mod v006_reactions;
pub mod v007_drafts;

use crate::error::StorageError;
use chrono::Utc;
use rusqlite::Connection;

pub use v001_initial::V001Initial;
pub use v002_users::V002Users;
pub use v003_channels::V003Channels;
pub use v004_messages::V004Messages;
pub use v005_attachments::V005Attachments;
pub use v006_reactions::V006Reactions;
pub use v007_drafts::V007Drafts;

/// Trait for database migrations.
///
/// Each migration has a unique version number and SQL to apply/revert changes.
pub trait Migration {
    /// Unique version number (e.g., 1, 2, 3...).
    ///
    /// Migrations are applied in order of version number.
    fn version(&self) -> u32;

    /// Human-readable description of what this migration does.
    fn description(&self) -> &'static str;

    /// SQL to apply this migration.
    ///
    /// Can contain multiple statements separated by semicolons.
    fn up(&self) -> &'static str;

    /// SQL to revert this migration (optional).
    ///
    /// Returns None if rollback is not supported.
    fn down(&self) -> Option<&'static str> {
        None
    }
}

/// Runs database migrations in order.
///
/// Tracks applied migrations in a `schema_version` table and only
/// applies migrations that haven't been run yet.
pub struct MigrationRunner {
    migrations: Vec<Box<dyn Migration>>,
}

impl MigrationRunner {
    /// Create a new empty migration runner.
    pub fn new() -> Self {
        Self {
            migrations: Vec::new(),
        }
    }

    /// Register a migration with the runner.
    ///
    /// Uses builder pattern for chaining.
    pub fn register(mut self, migration: impl Migration + 'static) -> Self {
        self.migrations.push(Box::new(migration));
        self
    }

    /// Run all pending migrations.
    ///
    /// Creates the schema_version table if it doesn't exist, then applies
    /// any migrations with version > current version in order.
    pub fn run(&self, conn: &Connection) -> Result<(), StorageError> {
        // Create schema_version table if not exists
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                description TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )?;

        // Get current version
        let current_version: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Sort migrations by version
        let mut sorted_migrations: Vec<_> = self.migrations.iter().collect();
        sorted_migrations.sort_by_key(|m| m.version());

        // Apply pending migrations
        for migration in sorted_migrations {
            let version = migration.version();
            if version > current_version {
                // Execute migration in a transaction
                let tx = conn.unchecked_transaction()?;

                // Execute the up SQL
                tx.execute_batch(migration.up())?;

                // Record migration in schema_version
                let now = Utc::now().to_rfc3339();
                tx.execute(
                    "INSERT INTO schema_version (version, description, applied_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![version, migration.description(), now],
                )?;

                tx.commit()?;

                tracing::info!(
                    version = version,
                    description = migration.description(),
                    "Applied migration"
                );
            }
        }

        Ok(())
    }
}

impl Default for MigrationRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a MigrationRunner with all migrations registered.
///
/// This is the standard way to get a runner with all migrations.
pub fn default_migrations() -> MigrationRunner {
    MigrationRunner::new()
        .register(V001Initial)
        .register(V002Users)
        .register(V003Channels)
        .register(V004Messages)
        .register(V005Attachments)
        .register(V006Reactions)
        .register(V007Drafts)
}

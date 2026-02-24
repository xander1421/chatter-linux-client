//! SQLCipher database connection.
//!
//! Encrypted database connection pool management using SQLCipher.

use crate::error::StorageError;
use crate::storage::migrations::default_migrations;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// SQLCipher database connection pool.
///
/// Provides thread-safe access to an encrypted SQLite database.
/// Uses a simplified single-connection model with mutex for v1.
pub struct Database {
    /// Connection with mutex for thread-safe access.
    conn: Arc<Mutex<Connection>>,
    /// Path to database file.
    path: PathBuf,
}

impl Database {
    /// Open an encrypted SQLCipher database.
    ///
    /// Creates the database file if it doesn't exist. The passphrase is used
    /// to encrypt all data at rest using SQLCipher.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the database file
    /// * `passphrase` - Encryption passphrase for SQLCipher
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Encryption` if the passphrase is invalid or
    /// encryption setup fails. Returns `StorageError::Database` for other
    /// database errors.
    pub fn open(path: &Path, passphrase: &str) -> Result<Self, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(path, flags)?;

        // Set SQLCipher encryption key
        // The passphrase must be set before any other operations
        conn.pragma_update(None, "key", passphrase)
            .map_err(|e| StorageError::Encryption(format!("Failed to set encryption key: {}", e)))?;

        // Verify the key is correct by selecting from sqlite_master
        // This works for both new and existing databases
        conn.execute_batch("SELECT count(*) FROM sqlite_master;")
            .map_err(|e| StorageError::Encryption(format!("Invalid encryption key: {}", e)))?;

        // Enable foreign key constraints
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Enable WAL mode for better concurrency (allows concurrent reads while writing)
        // This is essential for proactive DM session establishment which opens a second connection
        conn.pragma_update(None, "journal_mode", "WAL")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
        })
    }

    /// Get a locked connection for database operations.
    ///
    /// The returned guard holds the mutex lock until dropped.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::ConnectionPool` if the mutex is poisoned.
    pub fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.conn
            .lock()
            .map_err(|e| StorageError::ConnectionPool(format!("Connection lock poisoned: {}", e)))
    }

    /// Get the path to the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Explicitly close the database connection.
    ///
    /// This consumes the Database instance. Note that connections are also
    /// closed automatically when the Database is dropped.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::ConnectionPool` if the mutex is poisoned or
    /// if the connection cannot be unwrapped (other references exist).
    pub fn close(self) -> Result<(), StorageError> {
        // Try to get the inner connection
        let conn = Arc::try_unwrap(self.conn).map_err(|_| {
            StorageError::ConnectionPool("Cannot close: other references to connection exist".into())
        })?;

        let conn = conn.into_inner().map_err(|e| {
            StorageError::ConnectionPool(format!("Connection lock poisoned: {}", e))
        })?;

        // Close the connection explicitly
        conn.close().map_err(|(_, e)| StorageError::Database(e))?;

        Ok(())
    }

    /// Run all pending database migrations.
    ///
    /// Creates schema_version table if needed and applies any migrations
    /// that haven't been run yet. Safe to call multiple times.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Migration` if any migration fails.
    pub fn run_migrations(&self) -> Result<(), StorageError> {
        let conn = self.connection()?;
        let runner = default_migrations();
        runner.run(&conn)
    }
}

//! SQLCipher database connection.
//!
//! Encrypted database connection pool management using SQLCipher.
//!
//! Architecture:
//! - Single writer connection protected by Mutex (serializes writes)
//! - Pool of reader connections for concurrent reads (WAL mode enables this)
//! - Reader pool creates connections lazily and reuses them

use crate::error::StorageError;
use crate::storage::migrations::default_migrations;
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OpenFlags};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroize;

/// Number of reader connections in the pool.
const READER_POOL_SIZE: usize = 4;

/// SQLCipher database connection pool.
///
/// Provides thread-safe access to an encrypted SQLite database with
/// separate reader and writer connections for improved concurrency.
///
/// # Architecture
///
/// - **Writer**: Single connection protected by Mutex (serializes writes)
/// - **Readers**: Pool of read-only connections for concurrent SELECT queries
/// - **WAL mode**: Enables multiple concurrent readers while writing
///
/// # Usage
///
/// ```ignore
/// // For writes (INSERT, UPDATE, DELETE)
/// let conn = db.connection()?;
/// conn.execute("INSERT INTO ...", [])?;
///
/// // For reads (SELECT) - concurrent access
/// let conn = db.read_connection()?;
/// let mut stmt = conn.prepare("SELECT * FROM ...")?;
/// ```
pub struct Database {
    /// Writer connection with mutex for thread-safe access.
    writer: Arc<Mutex<Connection>>,
    /// Pool of reader connections.
    readers: Arc<ReaderPool>,
    /// Path to database file.
    path: PathBuf,
}

/// Pool of read-only connections for concurrent database reads.
///
/// Connections are created lazily up to `READER_POOL_SIZE`, then reused.
/// Each connection is initialized with the SQLCipher passphrase.
struct ReaderPool {
    /// Available reader connections.
    available: Mutex<VecDeque<Connection>>,
    /// Path for creating new connections.
    path: PathBuf,
    /// Passphrase for new connections (zeroized on drop).
    passphrase: Mutex<String>,
    /// Current number of connections (available + in-use).
    count: Mutex<usize>,
}

impl Drop for ReaderPool {
    fn drop(&mut self) {
        // Zeroize passphrase on drop
        self.passphrase.lock().zeroize();
    }
}

impl ReaderPool {
    fn new(path: PathBuf, passphrase: String) -> Self {
        Self {
            available: Mutex::new(VecDeque::with_capacity(READER_POOL_SIZE)),
            path,
            passphrase: Mutex::new(passphrase),
            count: Mutex::new(0),
        }
    }

    /// Get a copy of the passphrase for opening new connections.
    fn get_passphrase(&self) -> String {
        self.passphrase.lock().clone()
    }

    /// Acquire a reader connection from the pool.
    ///
    /// Returns an existing connection if available, otherwise creates a new one
    /// (up to READER_POOL_SIZE). If pool is exhausted, waits for a connection.
    fn acquire(&self) -> Result<PooledReader<'_>, StorageError> {
        // Try to get an available connection
        {
            let mut available = self.available.lock();
            if let Some(conn) = available.pop_front() {
                return Ok(PooledReader {
                    conn: Some(conn),
                    pool: self,
                });
            }
        }

        // Check if we can create a new connection
        let should_create = {
            let count = self.count.lock();
            *count < READER_POOL_SIZE
        };

        if should_create {
            let conn = self.open_reader()?;
            *self.count.lock() += 1;
            return Ok(PooledReader {
                conn: Some(conn),
                pool: self,
            });
        }

        // Pool exhausted - spin wait for a connection
        // This should be rare with proper pool sizing
        loop {
            std::thread::yield_now();
            let mut available = self.available.lock();
            if let Some(conn) = available.pop_front() {
                return Ok(PooledReader {
                    conn: Some(conn),
                    pool: self,
                });
            }
        }
    }

    /// Return a connection to the pool.
    fn release(&self, conn: Connection) {
        let mut available = self.available.lock();
        available.push_back(conn);
    }

    /// Open a new read-only SQLCipher connection.
    fn open_reader(&self) -> Result<Connection, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(&self.path, flags)?;

        // Set SQLCipher encryption key
        let passphrase = self.passphrase.lock();
        conn.pragma_update(None, "key", &*passphrase)
            .map_err(|e| StorageError::Encryption(format!("Failed to set reader key: {}", e)))?;

        // Verify the key
        conn.execute_batch("SELECT count(*) FROM sqlite_master;")
            .map_err(|e| StorageError::Encryption(format!("Invalid reader key: {}", e)))?;

        // Set WAL mode for reads
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Performance optimizations for large BLOB reads
        conn.pragma_update(None, "cache_size", "-65536")?; // 64MB cache
        conn.pragma_update(None, "mmap_size", "268435456")?; // 256MB mmap

        Ok(conn)
    }
}

/// A read-only connection borrowed from the pool.
///
/// Automatically returns to the pool when dropped.
pub struct PooledReader<'a> {
    conn: Option<Connection>,
    pool: &'a ReaderPool,
}

impl std::ops::Deref for PooledReader<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("connection taken")
    }
}

impl Drop for PooledReader<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
    }
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

        // Performance optimizations for large BLOB reads (attachments)
        // cache_size: negative value = KB, so -65536 = 64MB cache (default is ~2MB)
        // This significantly speeds up reading large attachments from SQLCipher
        conn.pragma_update(None, "cache_size", "-65536")?;

        // mmap_size: memory-map up to 256MB of the database file
        // This allows the OS to handle caching and can be faster than SQLite's cache
        conn.pragma_update(None, "mmap_size", "268435456")?;

        // Create reader pool for concurrent read access
        let readers = Arc::new(ReaderPool::new(
            path.to_path_buf(),
            passphrase.to_string(),
        ));

        Ok(Self {
            writer: Arc::new(Mutex::new(conn)),
            readers,
            path: path.to_path_buf(),
        })
    }

    /// Get a locked writer connection for database mutations.
    ///
    /// Use this for INSERT, UPDATE, DELETE operations.
    /// The returned guard holds the mutex lock until dropped.
    ///
    /// For read-only queries (SELECT), prefer `read_connection()` which
    /// allows concurrent access.
    pub fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        Ok(self.writer.lock())
    }

    /// Get a read-only connection for database queries.
    ///
    /// Use this for SELECT operations. Multiple readers can execute
    /// concurrently thanks to SQLite WAL mode.
    ///
    /// The connection is automatically returned to the pool when dropped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let conn = db.read_connection()?;
    /// let mut stmt = conn.prepare("SELECT * FROM messages WHERE channel_id = ?")?;
    /// let messages = stmt.query_map([channel_id], |row| { ... })?;
    /// ```
    pub fn read_connection(&self) -> Result<PooledReader<'_>, StorageError> {
        self.readers.acquire()
    }

    /// Get the path to the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a fresh read-only connection for streaming BLOB access.
    ///
    /// Unlike `read_connection()`, this connection is NOT pooled and must be
    /// closed by the caller. Use this for long-lived BLOB streaming where you
    /// need to hold the connection open for an extended period.
    ///
    /// # Use Case
    ///
    /// For streaming large attachments (e.g., 200MB video), you need to hold
    /// a connection open while reading chunks. Using a pooled connection would
    /// block other readers, so we open a dedicated connection instead.
    pub fn open_reader_connection(&self) -> Result<Connection, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(&self.path, flags)?;

        // Set SQLCipher encryption key from the reader pool
        // We need to get the passphrase from the pool
        let passphrase = self.readers.get_passphrase();
        conn.pragma_update(None, "key", &passphrase)
            .map_err(|e| StorageError::Encryption(format!("Failed to set reader key: {}", e)))?;

        // Verify the key
        conn.execute_batch("SELECT count(*) FROM sqlite_master;")
            .map_err(|e| StorageError::Encryption(format!("Invalid reader key: {}", e)))?;

        // Set WAL mode for reads
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Performance optimizations for large BLOB streaming
        conn.pragma_update(None, "cache_size", "-65536")?; // 64MB cache
        conn.pragma_update(None, "mmap_size", "268435456")?; // 256MB mmap

        Ok(conn)
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
        // Try to get the inner writer connection
        let writer = Arc::try_unwrap(self.writer).map_err(|_| {
            StorageError::ConnectionPool(
                "Cannot close: other references to writer connection exist".into(),
            )
        })?;

        let conn = writer.into_inner();

        // Close the writer connection explicitly
        conn.close().map_err(|(_, e)| StorageError::Database(e))?;

        // Reader pool connections are closed when the ReaderPool is dropped

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
